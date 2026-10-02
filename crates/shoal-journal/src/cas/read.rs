//! DB-independent, bounded, content-verified CAS reads.

use std::fmt;
use std::fs;
use std::io;
use std::io::Read as _;
use std::path::PathBuf;

use crate::{MAX_JOURNAL_CAS_MAX_BYTES, hex_bytes};

/// Full CAS materialization is intentionally narrower than the journal's
/// aggregate storage budget. Larger captured values remain available through
/// exact-length streaming and range APIs without one attacker-controlled
/// allocation.
pub const CAS_MATERIALIZE_MAX_BYTES: u64 = 256 * 1024 * 1024;
pub(crate) const CAS_COMPRESSED_OVERHEAD_BYTES: u64 = 1024 * 1024;

/// Machine-distinguishable CAS admission and integrity failures. Public read
/// APIs retain their established `io::Error`/`rusqlite::Error` signatures;
/// this value is stored as the I/O error's source (and can be downcast) rather
/// than flattening a safety boundary into an opaque string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CasReadError {
    MaterializationLimit { declared: u64, limit: u64 },
    CompressedLimit { actual: u64, limit: u64 },
    DecompressedLimit { actual: u64, limit: u64 },
    LengthMismatch { expected: u64, actual: u64 },
    HashMismatch { hash: String },
}

impl fmt::Display for CasReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaterializationLimit { declared, limit } => write!(
                f,
                "CAS blob declares {declared} bytes; full materialization limit is {limit} bytes"
            ),
            Self::CompressedLimit { actual, limit } => write!(
                f,
                "CAS blob compressed size {actual} exceeds the {limit}-byte admission limit"
            ),
            Self::DecompressedLimit { actual, limit } => write!(
                f,
                "CAS blob decompressed size exceeds its {limit}-byte limit (observed at least {actual})"
            ),
            Self::LengthMismatch { expected, actual } => write!(
                f,
                "CAS blob length mismatch: expected {expected} decompressed bytes, observed {actual}"
            ),
            Self::HashMismatch { hash } => {
                write!(
                    f,
                    "CAS blob {hash} failed integrity check: content hash mismatch"
                )
            }
        }
    }
}

impl std::error::Error for CasReadError {}

fn cas_read_io(error: CasReadError) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

/// A DB-independent handle to a CAS directory. Cloning is cheap; reads are
/// pure filesystem work and content-verified, so lazy values can retain one
/// without an SQLite connection or lifetime tie to the owning journal.
#[derive(Debug, Clone)]
pub struct Cas {
    pub(super) root: PathBuf,
}

impl Cas {
    fn blob_path(&self, hex: &str) -> PathBuf {
        self.root
            .join(&hex[0..2])
            .join(&hex[2..4])
            .join(format!("{hex}.zst"))
    }

    /// Read and decompress the CAS blob addressed by `hash`, verifying the
    /// decompressed bytes re-hash to `hash`. Unknown-length materialization is
    /// capped by [`CAS_MATERIALIZE_MAX_BYTES`].
    pub fn read(&self, hash: &str) -> io::Result<Vec<u8>> {
        let mut reader = self.open_verified(hash)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Materialize a blob whose authoritative uncompressed length is known.
    /// The declared size is admitted before allocation, and the decoder must
    /// end at exactly that boundary.
    pub fn read_exact(&self, hash: &str, expected_len: u64) -> io::Result<Vec<u8>> {
        if expected_len > CAS_MATERIALIZE_MAX_BYTES {
            return Err(cas_read_io(CasReadError::MaterializationLimit {
                declared: expected_len,
                limit: CAS_MATERIALIZE_MAX_BYTES,
            }));
        }
        let mut reader = self.open_verified_exact(hash, expected_len)?;
        let mut bytes = Vec::with_capacity(expected_len as usize);
        reader.read_to_end(&mut bytes)?;
        Ok(bytes)
    }

    /// Open a streaming decoder after verifying the full decompressed content
    /// hash in a bounded-memory first pass. Verification precedes delivery.
    pub fn open_verified(&self, hash: &str) -> io::Result<Box<dyn io::Read + Send>> {
        let actual_len = self.verify_stream(hash, None, CAS_MATERIALIZE_MAX_BYTES)?;
        let decoder = self.open_decoder(hash, actual_len)?;
        Ok(Box::new(ExactLengthReader::new(decoder, actual_len)))
    }

    /// Open a verified stream with an authoritative decompressed length. This
    /// is the large-value path: memory remains bounded and the decoder must end
    /// exactly at `expected_len`.
    pub fn open_verified_exact(
        &self,
        hash: &str,
        expected_len: u64,
    ) -> io::Result<Box<dyn io::Read + Send>> {
        if expected_len > MAX_JOURNAL_CAS_MAX_BYTES {
            return Err(cas_read_io(CasReadError::DecompressedLimit {
                actual: expected_len,
                limit: MAX_JOURNAL_CAS_MAX_BYTES,
            }));
        }
        self.verify_stream(hash, Some(expected_len), expected_len)?;
        let decoder = self.open_decoder(hash, expected_len)?;
        Ok(Box::new(ExactLengthReader::new(decoder, expected_len)))
    }

    fn verify_stream(&self, hash: &str, expected_len: Option<u64>, limit: u64) -> io::Result<u64> {
        let mut verify = self.open_decoder(hash, limit)?;
        let mut hasher = blake3::Hasher::new();
        let mut chunk = [0u8; 64 * 1024];
        let mut actual_len = 0u64;
        loop {
            let n = verify.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            actual_len = actual_len.saturating_add(n as u64);
            if actual_len > limit {
                return Err(cas_read_io(CasReadError::DecompressedLimit {
                    actual: actual_len,
                    limit,
                }));
            }
            hasher.update(&chunk[..n]);
        }
        if let Some(expected) = expected_len
            && actual_len != expected
        {
            return Err(cas_read_io(CasReadError::LengthMismatch {
                expected,
                actual: actual_len,
            }));
        }
        if !hasher
            .finalize()
            .to_hex()
            .as_str()
            .eq_ignore_ascii_case(hash)
        {
            return Err(cas_read_io(CasReadError::HashMismatch {
                hash: hash.to_owned(),
            }));
        }
        Ok(actual_len)
    }

    fn open_decoder(
        &self,
        hash: &str,
        decompressed_limit: u64,
    ) -> io::Result<Box<dyn io::Read + Send>> {
        if hex_bytes(hash).is_err() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("{hash} does not address a CAS blob"),
            ));
        }
        let file = fs::File::open(self.blob_path(hash))?;
        let compressed_len = file.metadata()?.len();
        let compressed_limit = decompressed_limit.saturating_add(CAS_COMPRESSED_OVERHEAD_BYTES);
        if compressed_len > compressed_limit {
            return Err(cas_read_io(CasReadError::CompressedLimit {
                actual: compressed_len,
                limit: compressed_limit,
            }));
        }
        let decoder = zstd::Decoder::new(io::BufReader::new(file))?;
        Ok(Box::new(decoder))
    }
}

struct ExactLengthReader<R> {
    inner: R,
    expected: u64,
    remaining: u64,
    finished: bool,
}

impl<R> ExactLengthReader<R> {
    fn new(inner: R, expected: u64) -> Self {
        Self {
            inner,
            expected,
            remaining: expected,
            finished: false,
        }
    }
}

impl<R: io::Read> io::Read for ExactLengthReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.finished || buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining > 0 {
            let admitted = buffer
                .len()
                .min(self.remaining.min(usize::MAX as u64) as usize);
            let read = self.inner.read(&mut buffer[..admitted])?;
            if read == 0 {
                return Err(cas_read_io(CasReadError::LengthMismatch {
                    expected: self.expected,
                    actual: self.expected - self.remaining,
                }));
            }
            self.remaining -= read as u64;
            return Ok(read);
        }
        let mut sentinel = [0u8; 1];
        if self.inner.read(&mut sentinel)? != 0 {
            return Err(cas_read_io(CasReadError::DecompressedLimit {
                actual: self.expected.saturating_add(1),
                limit: self.expected,
            }));
        }
        self.finished = true;
        Ok(0)
    }
}
