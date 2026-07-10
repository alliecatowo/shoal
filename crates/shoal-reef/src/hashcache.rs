//! Content hashing with an identity cache keyed by `(dev, inode, mtime, len)`.
//!
//! Re-hashing a binary on every spawn would be wasteful; a binary is identified
//! by its filesystem identity so a cache hit avoids the read. The key includes
//! `len` and `mtime` so an in-place rewrite (same inode) still invalidates.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Mutex;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct IdKey {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_ns: i64,
    len: u64,
}

/// A blake3 file-hash cache.
#[derive(Default)]
pub struct HashCache {
    map: Mutex<HashMap<IdKey, String>>,
}

impl HashCache {
    pub fn new() -> HashCache {
        HashCache::default()
    }

    /// Return the blake3 hex digest of the file at `path`, using the identity
    /// cache. Reads the file only on a cache miss.
    pub fn hash_file(&self, path: &Path) -> io::Result<String> {
        let meta = std::fs::metadata(path)?;
        let key = IdKey {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            len: meta.len(),
        };
        if let Some(h) = self.map.lock().unwrap().get(&key) {
            return Ok(h.clone());
        }
        let bytes = std::fs::read(path)?;
        let hex = blake3::hash(&bytes).to_hex().to_string();
        self.map.lock().unwrap().insert(key, hex.clone());
        Ok(hex)
    }
}

/// Hash bytes directly (no cache) — used for view-dir content addressing.
pub fn hash_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_caches() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin");
        std::fs::write(&p, b"hello world").unwrap();
        let cache = HashCache::new();
        let h1 = cache.hash_file(&p).unwrap();
        let h2 = cache.hash_file(&p).unwrap();
        assert_eq!(h1, h2);
        assert_eq!(h1, hash_bytes(b"hello world"));
    }

    #[test]
    fn rewrite_changes_hash() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bin");
        std::fs::write(&p, b"aaaa").unwrap();
        let cache = HashCache::new();
        let h1 = cache.hash_file(&p).unwrap();
        // Different length invalidates even if mtime resolution is coarse.
        std::fs::write(&p, b"bbbbbb").unwrap();
        let h2 = cache.hash_file(&p).unwrap();
        assert_ne!(h1, h2);
    }
}
