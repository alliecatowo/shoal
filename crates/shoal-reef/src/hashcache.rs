//! Content hashing with an identity cache keyed by
//! `(dev, inode, mtime, ctime, len)`.
//!
//! Re-hashing a binary on every spawn would be wasteful; a binary is identified
//! by its filesystem identity so a cache hit avoids the read. The key includes
//! `len`, `mtime`, and `ctime` so same-inode rewrites still invalidate even if
//! their byte length and modification time are preserved.

use std::collections::HashMap;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Maximum filesystem identities retained by one resolver. File hashes are an
/// advisory acceleration, so clearing the cache at the ceiling preserves
/// resolution results while bounding churn from replaced binaries.
const MAX_HASH_CACHE_ENTRIES: usize = 4_096;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct IdKey {
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_ns: i64,
    ctime: i64,
    ctime_ns: i64,
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
        let expected = std::fs::metadata(path)?;
        if !expected.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "hash target is not a regular file",
            ));
        }
        let mut file = std::fs::File::open(path)?;
        let meta = file.metadata()?;
        if !meta.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "opened hash target is not a regular file",
            ));
        }
        let key = IdKey {
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            ctime: meta.ctime(),
            ctime_ns: meta.ctime_nsec(),
            len: meta.len(),
        };
        if let Some(h) = self.lock_map().get(&key) {
            return Ok(h.clone());
        }
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        let hex = hasher.finalize().to_hex().to_string();
        let mut map = self.lock_map();
        if map.len() >= MAX_HASH_CACHE_ENTRIES && !map.contains_key(&key) {
            map.clear();
        }
        map.insert(key, hex.clone());
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
    fn filesystem_identity_churn_clears_the_advisory_cache_at_its_ceiling() {
        let cache = HashCache::new();
        let mut map = cache.lock_map();
        for ino in 0..MAX_HASH_CACHE_ENTRIES as u64 {
            map.insert(
                IdKey {
                    dev: 1,
                    ino,
                    mtime: 1,
                    mtime_ns: 0,
                    ctime: 1,
                    ctime_ns: 0,
                    len: 1,
                },
                "old".into(),
            );
        }
        drop(map);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current");
        std::fs::write(&path, b"authoritative bytes").unwrap();
        assert_eq!(
            cache.hash_file(&path).unwrap(),
            hash_bytes(b"authoritative bytes")
        );
        assert_eq!(cache.lock_map().len(), 1);
    }

    #[test]
    fn same_length_rewrite_with_restored_mtime_still_invalidates_on_ctime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bin");
        std::fs::write(&path, b"aaaa").unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        let cache = HashCache::new();
        let first = cache.hash_file(&path).unwrap();

        std::fs::write(&path, b"bbbb").unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();

        assert_ne!(cache.hash_file(&path).unwrap(), first);
        assert_eq!(cache.hash_file(&path).unwrap(), hash_bytes(b"bbbb"));
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

    #[test]
    fn production_hashing_streams_instead_of_reading_whole_files() {
        let production = include_str!("hashcache.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!production.contains("std::fs::read("));
        assert!(production.contains("[0u8; 64 * 1024]"));
    }
}
