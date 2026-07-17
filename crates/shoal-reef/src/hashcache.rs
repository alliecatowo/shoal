//! Content hashing with an identity cache keyed by `(dev, inode, mtime, len)`.
//!
//! Re-hashing a binary on every spawn would be wasteful; a binary is identified
//! by its filesystem identity so a cache hit avoids the read. The key includes
//! `len` and `mtime` so an in-place rewrite (same inode) still invalidates.

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

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

    fn lock_map(&self) -> MutexGuard<'_, HashMap<IdKey, String>> {
        match self.map.lock() {
            Ok(map) => map,
            Err(poisoned) => {
                // File hashes are an advisory acceleration only. Discard the
                // unknowable snapshot and rebuild entries from file contents.
                let mut map = poisoned.into_inner();
                map.clear();
                self.map.clear_poison();
                map
            }
        }
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
        if let Some(h) = self.lock_map().get(&key) {
            return Ok(h.clone());
        }
        let bytes = std::fs::read(path)?;
        let hex = blake3::hash(&bytes).to_hex().to_string();
        self.lock_map().insert(key, hex.clone());
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

    #[test]
    fn poisoned_hash_cache_discards_untrusted_entries_and_rebuilds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"trusted bytes").unwrap();
        let cache = std::sync::Arc::new(HashCache::new());
        cache.hash_file(&path).unwrap();

        let poison_target = cache.clone();
        let poisoner = std::thread::Builder::new()
            .name("poison-reef-hash-cache".into())
            .spawn(move || {
                let mut map = poison_target.map.lock().expect("hash cache starts healthy");
                let key = *map.keys().next().expect("initial hash populated the cache");
                map.insert(key, "poisoned-cache-value".into());
                panic!("inject hash cache poison");
            })
            .expect("spawn hash cache poisoner");
        assert!(poisoner.join().is_err());

        assert_eq!(
            cache.hash_file(&path).unwrap(),
            hash_bytes(b"trusted bytes")
        );
        assert!(!cache.map.is_poisoned());
        assert_eq!(cache.lock_map().len(), 1);
    }

    #[test]
    fn production_hash_cache_has_no_panicking_lock_access() {
        let production = include_str!("hashcache.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let compact = production.split_whitespace().collect::<String>();
        for forbidden in [".lock().unwrap(", ".lock().expect("] {
            assert!(
                !compact.contains(forbidden),
                "production hash cache synchronization contains `{forbidden}`"
            );
        }
    }
}
