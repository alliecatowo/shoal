//! The content-addressed store (CAS): blake3-addressed, zstd-compressed
//! output blobs, deduplicated on disk and tracked in the `blob` table.

use std::fs;
use std::io;
use std::io::Read as _;
use std::io::Seek as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::storage::DB_WRITE_RESERVE_BYTES;
use crate::{
    DEFAULT_JOURNAL_CAS_MAX_BYTES, DEFAULT_JOURNAL_DATABASE_MAX_BYTES, Journal,
    JournalStorageLimits, MAX_JOURNAL_CAS_MAX_BYTES, MAX_JOURNAL_DATABASE_MAX_BYTES, hex_bytes,
    io_to_sql, now_ns,
};

/// zstd compression level for CAS blobs (3 = the zstd default: fast, good ratio).
const ZSTD_LEVEL: i32 = 3;
/// Absolute allocation ceiling of the journal's generic range API. Kernel raw
/// pages use a smaller protocol wall; this protects direct embedders too.
pub const BLOB_RANGE_MAX_BYTES: usize = 64 * 1024;

const DEFAULT_OUTPUT_HARD_CAP: usize = 256 * 1024 * 1024;
/// Default SQLite `busy_timeout`: how long a writer blocks waiting for a
/// competing writer's lock before giving up with `SQLITE_BUSY`. The journal is
/// shared across processes (REPL + kernel + shoal-history all open the same
/// state dir), and the journaling call sites deliberately swallow errors, so a
/// zero timeout — rusqlite's default — silently drops a concurrent write *and*
/// its undo inverse. Five seconds comfortably covers a single-statement WAL
/// commit under contention.
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const TRUNCATION_MARKER: &[u8] = b"\n[shoal: output truncated; see journal metadata]\n";

#[derive(Debug, Clone, Copy)]
pub struct JournalOptions {
    pub output_hard_cap: usize,
    /// How long a write blocks on a busy database before failing (see
    /// [`DEFAULT_BUSY_TIMEOUT`]). Applied on every connection at open time.
    pub busy_timeout: Duration,
    /// Physical SQLite main+WAL admission ceiling. This is conservative
    /// headroom accounting, not a claim of a filesystem quota.
    pub database_max_bytes: u64,
    /// Logical uncompressed CAS admission ceiling (also reconciled against
    /// physical CAS bytes so crash-orphans cannot be wholly invisible).
    pub cas_max_bytes: u64,
}
impl Default for JournalOptions {
    fn default() -> Self {
        Self {
            output_hard_cap: DEFAULT_OUTPUT_HARD_CAP,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            database_max_bytes: env_budget(
                "SHOAL_JOURNAL_DATABASE_MAX_BYTES",
                DEFAULT_JOURNAL_DATABASE_MAX_BYTES,
                MAX_JOURNAL_DATABASE_MAX_BYTES,
            ),
            cas_max_bytes: env_budget(
                "SHOAL_JOURNAL_CAS_MAX_BYTES",
                DEFAULT_JOURNAL_CAS_MAX_BYTES,
                MAX_JOURNAL_CAS_MAX_BYTES,
            ),
        }
    }
}

impl JournalOptions {
    pub(crate) fn storage_limits(self) -> JournalStorageLimits {
        JournalStorageLimits {
            database_max_bytes: self
                .database_max_bytes
                .clamp(1, MAX_JOURNAL_DATABASE_MAX_BYTES),
            cas_max_bytes: self.cas_max_bytes.clamp(1, MAX_JOURNAL_CAS_MAX_BYTES),
        }
    }
}

fn env_budget(name: &str, default: u64, maximum: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
        .min(maximum)
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
        self.record_output_meta(id, kind, bytes).map(|(hex, _)| hex)
    }

    /// Like [`Journal::record_output`], but also returns the [`OutputMeta`]
    /// describing the stored blob when it was **truncated** to fit
    /// `output_hard_cap` (`None` when the bytes were stored whole).
    ///
    /// Callers that must not silently persist a truncated blob — undo snapshots
    /// above all, where the returned hash keys a replayable `RestoreBytes`
    /// inverse — use this to detect truncation and refuse rather than record a
    /// restore that would overwrite the user's file with partial+marker bytes.
    pub fn record_output_meta(
        &self,
        id: i64,
        kind: &str,
        bytes: &[u8],
    ) -> rusqlite::Result<(String, Option<OutputMeta>)> {
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
        let meta_json = meta
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let stored_len = i64::try_from(stored.len())
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let requested = DB_WRITE_RESERVE_BYTES
            .saturating_add(kind.len() as u64)
            .saturating_add(meta_json.as_ref().map_or(0, String::len) as u64);
        let path = self.blob_path(&hex);
        let write_result = self.with_database_admission(requested, |tx| {
            let admitted = self.admit_cas_growth(tx, hash.as_bytes(), stored.len() as u64)?;
            if admitted || !path.exists() {
                let parent = path.parent().expect("blob path always has a parent");
                fs::create_dir_all(parent).map_err(io_to_sql)?;
                let compressed =
                    zstd::encode_all(stored.as_slice(), ZSTD_LEVEL).map_err(io_to_sql)?;
                let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(io_to_sql)?;
                tmp.write_all(&compressed).map_err(io_to_sql)?;
                tmp.flush().map_err(io_to_sql)?;
                tmp.persist(&path).map_err(|error| io_to_sql(error.error))?;
            }
            let now = now_ns();
            tx.execute("INSERT OR IGNORE INTO blob(hash,stored_len,created_ns,last_access_ns) VALUES(?1,?2,?3,?3)",params![hash.as_bytes().as_slice(),stored_len,now])?;
            tx.execute(
                "INSERT INTO output (entry_id, kind, hash, len, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, kind, hash.as_bytes().as_slice(), stored_len, meta_json],
            )?;
            Ok(())
        });
        write_result?;
        Ok((hex, meta))
    }

    /// Fetch and decompress a CAS blob by its blake3 hex hash.
    ///
    /// Returns `Ok(None)` when the blob does not exist (including malformed hash
    /// strings, which cannot name a blob).
    ///
    /// The store is content-addressed, so the decompressed bytes are re-hashed
    /// and checked against the requested key before being returned. A mismatch
    /// (on-disk corruption / bit-rot / a swapped blob) is an integrity error
    /// rather than wrong bytes — this defends `undo` (`RestoreBytes`) and
    /// `blob.get` from ever acting on tampered content.
    pub fn read_blob(&self, hash: &str) -> rusqlite::Result<Option<Vec<u8>>> {
        // A hash that is not plain hex (or is too short to shard) cannot address
        // a blob; this also guards against path traversal.
        if hex_bytes(hash).is_err() {
            return Ok(None);
        }
        let compressed = match fs::read(self.blob_path(hash)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(io_to_sql(e)),
        };
        let bytes = zstd::decode_all(compressed.as_slice()).map_err(io_to_sql)?;
        // Integrity: content-addressed bytes MUST re-hash to their key.
        if !blake3::hash(&bytes)
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(hash)
        {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CAS blob {hash} failed integrity check: content hash mismatch"),
            )));
        }
        if let Ok(raw) = hex_bytes(hash) {
            self.touch_blob(&raw);
        }
        Ok(Some(bytes))
    }

    /// Read at most `length` uncompressed bytes beginning at `offset` without
    /// materializing the complete blob. Integrity is verified before any
    /// bytes are returned; compressed content is then streamed and discarded
    /// up to the requested offset using bounded memory.
    ///
    /// The returned tuple is `(total_uncompressed_len, page)`. An offset past
    /// EOF is clamped to EOF and returns an empty page. Missing/malformed hashes
    /// return `Ok(None)`, matching [`Self::read_blob`].
    pub fn read_blob_range(
        &self,
        hash: &str,
        offset: u64,
        length: usize,
    ) -> rusqlite::Result<Option<(u64, Vec<u8>)>> {
        let requested_offset = offset;
        let length = length.min(BLOB_RANGE_MAX_BYTES);
        if let Some(cached) = self.cached_blob_range(hash, requested_offset, length)? {
            return Ok(Some(cached));
        }
        let Some(total) = self.blob_len(hash)? else {
            return Ok(None);
        };
        let offset = offset.min(total);
        let wanted = total.saturating_sub(offset).min(length as u64) as usize;
        let mut reader = self.cas().open_verified(hash).map_err(io_to_sql)?;
        let skipped =
            io::copy(&mut reader.by_ref().take(offset), &mut io::sink()).map_err(io_to_sql)?;
        if skipped != offset {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CAS blob {hash} ended before its recorded length"),
            )));
        }
        let mut page = Vec::with_capacity(wanted);
        reader
            .take(wanted as u64)
            .read_to_end(&mut page)
            .map_err(io_to_sql)?;
        if page.len() != wanted {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CAS blob {hash} ended before its recorded length"),
            )));
        }
        if let Ok(raw) = hex_bytes(hash) {
            self.touch_blob(&raw);
        }
        self.blob_page_cache
            .borrow_mut()
            .insert(crate::BlobPageCacheEntry {
                hash: hash.to_ascii_lowercase(),
                offset: requested_offset,
                length,
                total,
                bytes: page.clone(),
            });
        Ok(Some((total, page)))
    }

    /// Return a previously verified exact range without reopening or
    /// decompressing the CAS object. Cache entries are bounded by both byte
    /// and count ceilings and are only served while the backing blob remains
    /// live in the journal and on disk.
    pub fn cached_blob_range(
        &self,
        hash: &str,
        offset: u64,
        length: usize,
    ) -> rusqlite::Result<Option<(u64, Vec<u8>)>> {
        let length = length.min(BLOB_RANGE_MAX_BYTES);
        let cached =
            self.blob_page_cache
                .borrow_mut()
                .get(&hash.to_ascii_lowercase(), offset, length);
        let Some((total, bytes)) = cached else {
            return Ok(None);
        };
        if self.blob_len(hash)? != Some(total) || !self.blob_path(hash).is_file() {
            return Ok(None);
        }
        if let Ok(raw) = hex_bytes(hash) {
            self.touch_blob(&raw);
        }
        Ok(Some((total, bytes)))
    }

    /// The stored (uncompressed) byte length of the CAS blob addressed by
    /// `hash`, read from the `blob` table alone — no file open, no decode. `None`
    /// when no such blob is tracked (unknown/malformed hash). This is the cheap
    /// metadata a lazy, ref-backed [`crate::Cas`] value answers `.len` from when
    /// resolving a bare `val:blake3:<hash>` ref (site/content/internals/language-conformance-contract.md in-language dispatch):
    /// the true content length without ever materializing it.
    pub fn blob_len(&self, hash: &str) -> rusqlite::Result<Option<u64>> {
        let raw = match hex_bytes(hash) {
            Ok(raw) => raw,
            Err(()) => return Ok(None),
        };
        match self.conn.query_row(
            "SELECT stored_len FROM blob WHERE hash = ?1",
            params![raw.as_slice()],
            |row| row.get::<_, i64>(0),
        ) {
            Ok(len) => Ok(Some(len as u64)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Absolute path of the CAS blob for a hex hash:
    /// `<cas>/<hex[0..2]>/<hex[2..4]>/<hex>.zst`.
    pub(crate) fn blob_path(&self, hex: &str) -> PathBuf {
        self.cas_root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.zst"))
    }

    /// A cheap, cloneable, DB-independent reader over this journal's CAS
    /// directory. Reading a blob is pure filesystem work (open + zstd-decode +
    /// integrity check), so the returned [`Cas`] needs no SQLite connection and
    /// is `Send + Sync` — the shape a lazy, ref-backed value's loader needs
    /// (site/content/internals/language-conformance-contract.md). It shares the same on-disk store, so blobs written via
    /// [`Journal::record_output`] / [`Journal::ingest_spill`] are readable
    /// through it.
    pub fn cas(&self) -> Cas {
        Cas {
            root: self.cas_root.clone(),
        }
    }

    /// Directory value-position captures spill oversized stdout into before it
    /// is adopted via [`Journal::ingest_spill`] (site/content/internals/language-conformance-contract.md). Co-located with the
    /// CAS (a sibling of `cas/` under the state dir) so it shares the store's
    /// filesystem and lifetime (an in-memory journal's temp dir cleans it up).
    /// Created on demand.
    pub fn spill_dir(&self) -> io::Result<PathBuf> {
        let dir = self
            .cas_root
            .parent()
            .unwrap_or(self.cas_root.as_path())
            .join("spill");
        fs::create_dir_all(&dir)?;
        Ok(dir)
    }

    /// Adopt an already-written spill file (from [`Journal::spill_dir`]) into
    /// the CAS as a compressed, blake3-addressed blob (site/content/internals/language-conformance-contract.md). `hash` and
    /// `len` come from [`shoal_exec::CaptureSpill`] — the blake3 and byte length
    /// of the file's contents. The blob is written (zstd-streamed, so RAM stays
    /// bounded) only if not already present; a `blob` row is recorded and,
    /// when `pin` is set, the blob is pinned so GC keeps it while the
    /// in-language ref-backed value is live. The source file is removed on
    /// success.
    pub fn ingest_spill(
        &self,
        src: &Path,
        hash: &str,
        len: u64,
        pin: bool,
    ) -> rusqlite::Result<()> {
        let raw = hex_bytes(hash)
            .map_err(|_| rusqlite::Error::InvalidParameterName("invalid hash".into()))?;
        let stored_len = i64::try_from(len)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let mut infile = fs::File::open(src).map_err(io_to_sql)?;
        let metadata = infile.metadata().map_err(io_to_sql)?;
        if !metadata.is_file() || metadata.len() != len {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture spill length does not match its declared length",
            )));
        }
        verify_spill(&mut infile, hash, len)?;
        let path = self.blob_path(hash);
        self.with_database_admission(DB_WRITE_RESERVE_BYTES, |tx| {
            let admitted = self.admit_cas_growth(tx, &raw, len)?;
            if admitted || !path.exists() {
                infile.rewind().map_err(io_to_sql)?;
                let parent = path.parent().expect("blob path always has a parent");
                fs::create_dir_all(parent).map_err(io_to_sql)?;
                let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(io_to_sql)?;
                let mut verified = HashingReader::new(&mut infile);
                zstd::stream::copy_encode(&mut verified, &mut tmp, ZSTD_LEVEL)
                    .map_err(io_to_sql)?;
                verified.finish(hash, len)?;
                tmp.flush().map_err(io_to_sql)?;
                tmp.persist(&path)
                    .map_err(|error| io_to_sql(error.error))?;
            }
            let now = now_ns();
            tx.execute(
                "INSERT OR IGNORE INTO blob(hash,stored_len,created_ns,last_access_ns) VALUES(?1,?2,?3,?3)",
                params![raw.as_slice(), stored_len, now],
            )?;
            if pin {
                tx.execute(
                    "INSERT OR IGNORE INTO pin(hash) VALUES(?1)",
                    params![raw.as_slice()],
                )?;
            }
            Ok(())
        })?;
        // Best-effort cleanup: the blob is safely in the CAS now.
        let _ = fs::remove_file(src);
        Ok(())
    }

    fn touch_blob(&self, raw: &[u8]) {
        // LRU freshness is optional metadata. Near exhaustion or under writer
        // contention, serving already-verified bytes is more important than a
        // timestamp update, and the read path must not bypass write admission.
        let _ = self.with_database_admission(DB_WRITE_RESERVE_BYTES, |tx| {
            tx.execute(
                "UPDATE blob SET last_access_ns=?1 WHERE hash=?2",
                params![now_ns(), raw],
            )?;
            Ok(())
        });
    }
}

fn verify_spill(
    file: &mut fs::File,
    expected_hash: &str,
    expected_len: u64,
) -> rusqlite::Result<()> {
    file.rewind().map_err(io_to_sql)?;
    let mut reader = HashingReader::new(file);
    io::copy(&mut reader, &mut io::sink()).map_err(io_to_sql)?;
    reader.finish(expected_hash, expected_len)
}

struct HashingReader<R> {
    inner: R,
    hasher: blake3::Hasher,
    bytes: u64,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: blake3::Hasher::new(),
            bytes: 0,
        }
    }

    fn finish(self, expected_hash: &str, expected_len: u64) -> rusqlite::Result<()> {
        let actual_hash = self.hasher.finalize().to_hex();
        if self.bytes != expected_len || !actual_hash.as_str().eq_ignore_ascii_case(expected_hash) {
            return Err(io_to_sql(io::Error::new(
                io::ErrorKind::InvalidData,
                "capture spill content does not match its declared hash/length",
            )));
        }
        Ok(())
    }
}

impl<R: io::Read> io::Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes = self
            .bytes
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("capture spill length overflow"))?;
        self.hasher.update(&buf[..read]);
        Ok(read)
    }
}

/// A DB-independent handle to a CAS directory (see [`Journal::cas`]). Cloning is
/// cheap (one `PathBuf`); reads are pure filesystem work and content-verified,
/// so a lazy [`shoal_value`]-side value can hold one as its loader without any
/// SQLite connection or lifetime tie to the owning [`Journal`].
#[derive(Debug, Clone)]
pub struct Cas {
    root: PathBuf,
}

impl Cas {
    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.zst"))
    }

    /// Read and decompress the CAS blob addressed by `hash`, verifying the
    /// decompressed bytes re-hash to `hash` (the same integrity guard as
    /// [`Journal::read_blob`]). A missing blob or malformed hash is a
    /// `NotFound` error; a hash mismatch is `InvalidData`.
    pub fn read(&self, hash: &str) -> io::Result<Vec<u8>> {
        if hex_bytes(hash).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{hash} does not address a CAS blob"),
            ));
        }
        let compressed = fs::read(self.blob_path(hash))?;
        let bytes = zstd::decode_all(compressed.as_slice())?;
        if !blake3::hash(&bytes)
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(hash)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CAS blob {hash} failed integrity check: content hash mismatch"),
            ));
        }
        Ok(bytes)
    }

    /// Open a streaming decoder after verifying the full decompressed content
    /// hash in a bounded-memory first pass. Verification precedes delivery, so
    /// a consumer that intentionally stops early never observes bytes from a
    /// corrupt content-addressed blob. The second pass trades additional disk
    /// and decompression work for bounded memory and fail-closed integrity.
    pub fn open_verified(&self, hash: &str) -> io::Result<Box<dyn io::Read + Send>> {
        let mut verify = self.open_decoder(hash)?;
        let mut hasher = blake3::Hasher::new();
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let n = verify.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            hasher.update(&chunk[..n]);
        }
        if !hasher
            .finalize()
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(hash)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("CAS blob {hash} failed integrity check: content hash mismatch"),
            ));
        }
        self.open_decoder(hash)
    }

    fn open_decoder(&self, hash: &str) -> io::Result<Box<dyn io::Read + Send>> {
        if hex_bytes(hash).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{hash} does not address a CAS blob"),
            ));
        }
        let file = fs::File::open(self.blob_path(hash))?;
        let decoder = zstd::Decoder::new(io::BufReader::new(file))?;
        Ok(Box::new(decoder))
    }
}
