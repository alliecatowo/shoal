use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;
use zeroize::{Zeroize, Zeroizing};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenMeta {
    pub id: String,
    pub principal: String,
    pub profile: String,
    pub caps: Vec<String>,
    pub created_ns: i64,
    pub expires_ns: Option<i64>,
    pub revoked_ns: Option<i64>,
}
#[derive(Serialize, Deserialize)]
struct Stored {
    version: u32,
    key: String,
    tokens: Vec<StoredToken>,
}

impl Drop for Stored {
    fn drop(&mut self) {
        self.key.zeroize();
    }
}
#[derive(Serialize, Deserialize)]
struct StoredToken {
    #[serde(flatten)]
    meta: TokenMeta,
    digest: String,
}
pub struct TokenStore {
    path: PathBuf,
    key: Zeroizing<[u8; 32]>,
    tokens: Vec<StoredToken>,
}

impl TokenStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "token store needs a parent directory",
            )
        })?;
        fs::create_dir_all(parent)?;
        secure_dir(parent)?;

        // Initialization participates in the same transaction lock as writes:
        // simultaneous first-openers must share one keyed-hash key, not race to
        // replace the store with incompatible keys.
        let lock_target = path.clone();
        with_exclusive_lock(&lock_target, || {
            if path.exists() {
                secure_file(&path)?;
                let (key, tokens) = load_unlocked(&path)?;
                Ok(Self { path, key, tokens })
            } else {
                let mut key = Zeroizing::new([0; 32]);
                getrandom::fill(&mut *key).map_err(|e| io::Error::other(e.to_string()))?;
                let store = Self {
                    path,
                    key,
                    tokens: vec![],
                };
                store.persist_unlocked()?;
                Ok(store)
            }
        })
    }

    /// Create and persist a bearer while holding the whole read/modify/write
    /// transaction lock. The store is reloaded after lock acquisition so a
    /// token created by another process can never be overwritten by a stale
    /// in-memory snapshot.
    pub fn create(
        &mut self,
        principal: String,
        profile: String,
        caps: Vec<String>,
        ttl_ns: Option<i64>,
    ) -> io::Result<(String, TokenMeta)> {
        let path = self.path.clone();
        with_exclusive_lock(&path, || {
            self.reload_unlocked()?;
            let mut secret = Zeroizing::new([0; 32]);
            getrandom::fill(&mut *secret).map_err(|e| io::Error::other(e.to_string()))?;
            // The returned bearer is necessarily an ordinary String for API
            // compatibility; callers own and must dispose of that final copy.
            let bearer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*secret);
            let now = now_ns();
            let digest = self.digest(bearer.as_bytes());
            let meta = TokenMeta {
                id: hex(&digest[..8]),
                principal,
                profile,
                caps,
                created_ns: now,
                expires_ns: ttl_ns.map(|t| now.saturating_add(t)),
                revoked_ns: None,
            };
            self.tokens.push(StoredToken {
                meta: meta.clone(),
                digest: hex(&digest),
            });
            self.persist_unlocked()?;
            Ok((bearer, meta))
        })
    }

    /// Validate against a freshly locked disk snapshot and surface storage or
    /// integrity failures to callers that need to distinguish them from an
    /// invalid bearer. No cached authority is consulted on failure.
    pub fn validate_checked(&self, bearer: &str) -> io::Result<Option<TokenMeta>> {
        let (key, tokens) = with_shared_lock(&self.path, || load_unlocked(&self.path))?;
        let digest = digest_with(&key, bearer.as_bytes());
        let now = now_ns();
        Ok(tokens
            .iter()
            .find(|t| {
                // `load_unlocked` validates every digest before exposing the
                // snapshot, so conversion cannot fail here.
                unhex(&t.digest).is_ok_and(|stored| bool::from(stored.as_slice().ct_eq(&digest)))
            })
            .filter(|t| t.meta.revoked_ns.is_none() && t.meta.expires_ns.is_none_or(|e| e > now))
            .map(|t| t.meta.clone()))
    }

    /// Compatibility authentication API. Storage, locking, and integrity
    /// errors deliberately collapse to unauthenticated; they never restore a
    /// token from the startup snapshot.
    pub fn validate(&self, bearer: &str) -> Option<TokenMeta> {
        self.validate_checked(bearer).ok().flatten()
    }

    /// Refresh the status of a token that was already authenticated with its
    /// bearer. This deliberately accepts the prior private [`TokenMeta`]
    /// record rather than a public token id alone, so it cannot become an
    /// alternate bearer-authentication path. The disk snapshot is fresh and
    /// storage failures, revocation, expiry, or identity replacement all fail
    /// closed.
    pub fn refresh_authenticated_checked(
        &self,
        attached: &TokenMeta,
    ) -> io::Result<Option<TokenMeta>> {
        let (_, tokens) = with_shared_lock(&self.path, || load_unlocked(&self.path))?;
        let now = now_ns();
        Ok(tokens
            .into_iter()
            .find(|token| {
                token.meta.id == attached.id
                    && token.meta.created_ns == attached.created_ns
                    && token.meta.principal == attached.principal
                    && token.meta.profile == attached.profile
                    && token.meta.caps == attached.caps
                    && token.meta.expires_ns == attached.expires_ns
            })
            .filter(|token| {
                token.meta.revoked_ns.is_none()
                    && token.meta.expires_ns.is_none_or(|expires| expires > now)
            })
            .map(|token| token.meta))
    }

    /// Fail-closed compatibility wrapper around
    /// [`Self::refresh_authenticated_checked`].
    pub fn refresh_authenticated(&self, attached: &TokenMeta) -> Option<TokenMeta> {
        self.refresh_authenticated_checked(attached).ok().flatten()
    }

    /// Fallible, fresh list for callers that need storage errors surfaced.
    pub fn try_list(&self) -> io::Result<Vec<TokenMeta>> {
        with_shared_lock(&self.path, || {
            let (_, tokens) = load_unlocked(&self.path)?;
            Ok(tokens.into_iter().map(|t| t.meta).collect())
        })
    }

    /// Compatibility list. New code should prefer [`Self::try_list`] so an
    /// on-disk integrity/I/O error is not hidden. On failure this returns an
    /// empty list rather than stale startup state.
    pub fn list(&self) -> Vec<TokenMeta> {
        self.try_list().unwrap_or_default()
    }

    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        let path = self.path.clone();
        with_exclusive_lock(&path, || {
            self.reload_unlocked()?;
            let Some(t) = self.tokens.iter_mut().find(|t| t.meta.id == id) else {
                return Ok(false);
            };
            t.meta.revoked_ns = Some(now_ns());
            self.persist_unlocked()?;
            Ok(true)
        })
    }

    fn reload_unlocked(&mut self) -> io::Result<()> {
        let (key, tokens) = load_unlocked(&self.path)?;
        self.key = key;
        self.tokens = tokens;
        Ok(())
    }

    fn digest(&self, secret: &[u8]) -> [u8; 32] {
        digest_with(&self.key, secret)
    }

    fn persist_unlocked(&self) -> io::Result<()> {
        let mut doc = Stored {
            version: 1,
            key: base64::engine::general_purpose::STANDARD.encode(*self.key),
            tokens: self
                .tokens
                .iter()
                .map(|t| StoredToken {
                    meta: t.meta.clone(),
                    digest: t.digest.clone(),
                })
                .collect(),
        };
        let bytes = Zeroizing::new(serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?);
        // Wipe the additional base64 key copy as soon as serialization is done.
        doc.key.zeroize();
        atomic_replace(&self.path, &bytes)
    }
}

fn load_unlocked(path: &Path) -> io::Result<(Zeroizing<[u8; 32]>, Vec<StoredToken>)> {
    secure_file(path)?;
    let bytes = Zeroizing::new(fs::read(path)?);
    let mut doc: Stored = serde_json::from_slice(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if doc.version != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported token store version",
        ));
    }
    let raw = Zeroizing::new(
        base64::engine::general_purpose::STANDARD
            .decode(&doc.key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
    );
    let key = Zeroizing::new(
        raw.as_slice()
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid token key"))?,
    );
    let tokens = std::mem::take(&mut doc.tokens);
    validate_tokens(&tokens)?;
    Ok((key, tokens))
}

fn validate_tokens(tokens: &[StoredToken]) -> io::Result<()> {
    let mut ids = BTreeSet::new();
    let mut digests = BTreeSet::new();
    for token in tokens {
        let digest = unhex(&token.digest).map_err(|()| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid token digest encoding")
        })?;
        if digest.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid token digest length",
            ));
        }
        if token.meta.id != hex(&digest[..8]) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "token id does not match digest",
            ));
        }
        if !ids.insert(&token.meta.id) || !digests.insert(&token.digest) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "duplicate token identity",
            ));
        }
    }
    Ok(())
}

fn digest_with(key: &[u8; 32], secret: &[u8]) -> [u8; 32] {
    *blake3::keyed_hash(key, secret).as_bytes()
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let p = lock_path(path);
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&p)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn with_exclusive_lock<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = open_lock_file(path)?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    f()
}

fn with_shared_lock<T>(path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = open_lock_file(path)?;
    let lock = fd_lock::RwLock::new(file);
    let _guard = lock.read()?;
    f()
}

fn secure_dir(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

fn secure_file(path: &Path) -> io::Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "token store needs a parent directory",
        )
    })?;
    let tmp = tempfile_path(path);
    // The exclusive store lock permits one active writer, but a crashed writer
    // can leave this process-specific file behind.
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&tmp, path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn tempfile_path(path: &Path) -> PathBuf {
    path.with_extension(format!("tmp.{}", std::process::id()))
}
fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(i64::MAX as u128) as i64
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}
fn unhex(s: &str) -> Result<Vec<u8>, ()> {
    fn nibble(byte: u8) -> Result<u8, ()> {
        match byte {
            b'0'..=b'9' => Ok(byte - b'0'),
            b'a'..=b'f' => Ok(byte - b'a' + 10),
            b'A'..=b'F' => Ok(byte - b'A' + 10),
            _ => Err(()),
        }
    }

    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(());
    }
    bytes
        .chunks_exact(2)
        .map(|pair| Ok((nibble(pair[0])? << 4) | nibble(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::sync::Arc;

    #[test]
    fn secrets_never_persist_and_revoke_expiry_work() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tokens.json");
        let mut s = TokenStore::open(&p).unwrap();
        let (secret, m) = s
            .create("agent:a".into(), "dev".into(), vec!["fs.read".into()], None)
            .unwrap();
        assert!(
            !String::from_utf8(fs::read(&p).unwrap())
                .unwrap()
                .contains(&secret)
        );
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(s.validate(&secret).unwrap().principal, "agent:a");
        assert!(s.revoke(&m.id).unwrap());
        assert!(s.validate(&secret).is_none());
        let (expired, _) = s
            .create("agent:b".into(), "x".into(), vec![], Some(-1))
            .unwrap();
        assert!(s.validate(&expired).is_none());
    }

    #[test]
    fn reopen_roundtrip_and_cross_store_revoke_are_fresh() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("tokens.json");
        let mut creator = TokenStore::open(&p).unwrap();
        let (bearer, meta) = creator
            .create("agent:roundtrip".into(), "dev".into(), vec![], None)
            .unwrap();

        let reader = TokenStore::open(&p).unwrap();
        assert_eq!(reader.validate(&bearer).unwrap().id, meta.id);
        assert_eq!(reader.refresh_authenticated(&meta).unwrap().id, meta.id);
        let mut revoker = TokenStore::open(&p).unwrap();
        assert!(revoker.revoke(&meta.id).unwrap());
        // validate reloads under a shared file lock, so the already-open reader
        // observes another process/store's revoke rather than trusting a stale
        // startup snapshot.
        assert!(reader.validate(&bearer).is_none());
        assert!(reader.refresh_authenticated(&meta).is_none());
    }

    #[test]
    fn malformed_snapshot_is_typed_and_never_reuses_cached_authority() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tokens.json");
        let mut creator = TokenStore::open(&path).unwrap();
        let (bearer, meta) = creator
            .create("agent:cached".into(), "dev".into(), vec![], None)
            .unwrap();
        let reader = Arc::new(TokenStore::open(&path).unwrap());

        let mut stored: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        // Even byte length plus a multibyte character exercised a prior UTF-8
        // slicing panic in the hex decoder.
        stored["tokens"][0]["digest"] = serde_json::Value::String("aéx".into());
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

        for _ in 0..8 {
            assert_eq!(
                reader.validate_checked(&bearer).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert!(reader.validate(&bearer).is_none());
            assert_eq!(
                reader
                    .refresh_authenticated_checked(&meta)
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidData
            );
            assert!(reader.refresh_authenticated(&meta).is_none());
            assert!(reader.try_list().is_err());
            assert!(reader.list().is_empty());
        }

        let workers: Vec<_> = (0..8)
            .map(|_| {
                let reader = Arc::clone(&reader);
                let bearer = bearer.clone();
                std::thread::spawn(move || {
                    for _ in 0..16 {
                        assert!(reader.validate_checked(&bearer).is_err());
                        assert!(reader.validate(&bearer).is_none());
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("authentication worker must not panic");
        }
    }

    #[test]
    fn production_has_no_raw_panicking_lock_access() {
        let production = include_str!("lib.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or_default();
        let compact: String = production.chars().filter(|c| !c.is_whitespace()).collect();
        for forbidden in [
            ".lock().unwrap()",
            ".read().unwrap()",
            ".write().unwrap()",
            ".lock().expect(",
            ".read().expect(",
            ".write().expect(",
        ] {
            assert!(
                !compact.contains(forbidden),
                "production auth synchronization must return typed errors: {forbidden}"
            );
        }
    }

    #[test]
    fn token_store_permissions_are_owner_only() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state").join("tokens.json");
        let mut store = TokenStore::open(&p).unwrap();
        store
            .create("agent:a".into(), "dev".into(), vec![], None)
            .unwrap();
        assert_eq!(
            fs::metadata(p.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        for path in [&p, &lock_path(&p)] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600,
                "{} must be owner-only",
                path.display()
            );
        }
    }

    #[test]
    fn concurrent_process_writers_preserve_every_token() {
        const WORKER_ENV: &str = "SHOAL_AUTH_LOCK_TEST_WORKER";
        const PATH_ENV: &str = "SHOAL_AUTH_LOCK_TEST_PATH";

        if let Some(principal) = std::env::var_os(WORKER_ENV) {
            let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("worker store path"));
            let mut store = TokenStore::open(path).unwrap();
            store
                .create(
                    principal.to_string_lossy().into_owned(),
                    "test".into(),
                    vec![],
                    None,
                )
                .unwrap();
            return;
        }

        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("state").join("tokens.json");
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for i in 0..12 {
            children.push(
                Command::new(&exe)
                    .args([
                        "--exact",
                        "tests::concurrent_process_writers_preserve_every_token",
                        "--nocapture",
                    ])
                    .env(WORKER_ENV, format!("agent:{i}"))
                    .env(PATH_ENV, &path)
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            assert!(child.wait_with_output().unwrap().status.success());
        }

        let store = TokenStore::open(&path).unwrap();
        let mut principals: Vec<_> = store
            .try_list()
            .unwrap()
            .into_iter()
            .map(|m| m.principal)
            .collect();
        principals.sort();
        assert_eq!(principals.len(), 12, "a concurrent update was lost");
        for i in 0..12 {
            assert!(principals.contains(&format!("agent:{i}")));
        }
    }
}
