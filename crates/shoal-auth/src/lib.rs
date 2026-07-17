use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use subtle::ConstantTimeEq;

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
#[derive(Clone, Serialize, Deserialize)]
struct StoredToken {
    #[serde(flatten)]
    meta: TokenMeta,
    digest: String,
}
/// Bearer-token store persisted as one JSON document. Every mutation
/// (`create`, `revoke`, and the one-time bootstrap inside `open`) reloads the
/// on-disk token list under an exclusive interprocess file lock, applies its
/// change, and writes back before releasing the lock, so two processes racing
/// to create/revoke tokens against the same file cannot silently lose one
/// another's update (HR-I3; see "Bearer token storage" in
/// `site/content/internals/security-threat-model.md`). `validate`/`list`
/// still read this instance's in-memory snapshot rather than the disk, which
/// is the separate, already-documented revocation-latency limitation, not
/// the lost-update bug this lock fixes.
pub struct TokenStore {
    path: PathBuf,
    key: [u8; 32],
    tokens: Vec<StoredToken>,
}

impl TokenStore {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_owned();
        if let Some(p) = path.parent() {
            fs::create_dir_all(p)?;
        }
        let lock_path = lock_path_for(&path);
        // The one-time key bootstrap races the same way a mutation does: two
        // first-openers must never install different keys, so it shares the
        // exclusive lock rather than a bare existence check.
        with_exclusive_lock(&lock_path, || {
            if path.exists() {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
                let bytes = fs::read(&path)?;
                let doc: Stored = serde_json::from_slice(&bytes)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let raw = base64::engine::general_purpose::STANDARD
                    .decode(doc.key)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                let key: [u8; 32] = raw
                    .try_into()
                    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid token key"))?;
                Ok(Self {
                    path: path.clone(),
                    key,
                    tokens: doc.tokens,
                })
            } else {
                let mut key = [0; 32];
                getrandom::fill(&mut key).map_err(|e| io::Error::other(e.to_string()))?;
                let store = Self {
                    path: path.clone(),
                    key,
                    tokens: vec![],
                };
                store.persist_tokens_unlocked(&store.tokens)?;
                Ok(store)
            }
        })
    }
    pub fn create(
        &mut self,
        principal: String,
        profile: String,
        caps: Vec<String>,
        ttl_ns: Option<i64>,
    ) -> io::Result<(String, TokenMeta)> {
        let mut secret = [0; 32];
        getrandom::fill(&mut secret).map_err(|e| io::Error::other(e.to_string()))?;
        let bearer = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
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
        let new_token = StoredToken {
            meta: meta.clone(),
            digest: hex(&digest),
        };
        let lock_path = self.lock_path();
        with_exclusive_lock(&lock_path, || {
            let mut tokens = self.reload_tokens_unlocked()?;
            tokens.push(new_token.clone());
            self.persist_tokens_unlocked(&tokens)?;
            self.tokens = tokens;
            Ok(())
        })?;
        Ok((bearer, meta))
    }
    pub fn validate(&self, bearer: &str) -> Option<TokenMeta> {
        let digest = self.digest(bearer.as_bytes());
        let now = now_ns();
        self.tokens
            .iter()
            .find(|t| {
                let Ok(stored) = unhex(&t.digest) else {
                    return false;
                };
                stored.len() == 32 && bool::from(stored.as_slice().ct_eq(&digest))
            })
            .filter(|t| t.meta.revoked_ns.is_none() && t.meta.expires_ns.is_none_or(|e| e > now))
            .map(|t| t.meta.clone())
    }
    pub fn list(&self) -> Vec<TokenMeta> {
        self.tokens.iter().map(|t| t.meta.clone()).collect()
    }
    pub fn revoke(&mut self, id: &str) -> io::Result<bool> {
        let lock_path = self.lock_path();
        with_exclusive_lock(&lock_path, || {
            let mut tokens = self.reload_tokens_unlocked()?;
            let Some(t) = tokens.iter_mut().find(|t| t.meta.id == id) else {
                return Ok(false);
            };
            t.meta.revoked_ns = Some(now_ns());
            self.persist_tokens_unlocked(&tokens)?;
            self.tokens = tokens;
            Ok(true)
        })
    }
    fn digest(&self, secret: &[u8]) -> [u8; 32] {
        *blake3::keyed_hash(&self.key, secret).as_bytes()
    }
    fn lock_path(&self) -> PathBuf {
        lock_path_for(&self.path)
    }
    /// Read the token list currently on disk (the authoritative state after
    /// any other process's writes), falling back to this instance's
    /// in-memory list only if the file has not been persisted yet. Must be
    /// called while holding this store's exclusive lock.
    fn reload_tokens_unlocked(&self) -> io::Result<Vec<StoredToken>> {
        if !self.path.exists() {
            return Ok(self.tokens.clone());
        }
        let bytes = fs::read(&self.path)?;
        let doc: Stored = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        Ok(doc.tokens)
    }
    /// Atomically write `tokens` (not necessarily `self.tokens`, so a
    /// reload-merge-persist cycle can write the freshly reloaded list). Must
    /// be called while holding this store's exclusive lock.
    fn persist_tokens_unlocked(&self, tokens: &[StoredToken]) -> io::Result<()> {
        if let Some(p) = self.path.parent() {
            fs::create_dir_all(p)?;
        }
        let doc = Stored {
            version: 1,
            key: base64::engine::general_purpose::STANDARD.encode(self.key),
            tokens: tokens.to_vec(),
        };
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));
        let _ = fs::remove_file(&tmp);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&doc).map_err(io::Error::other)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, &self.path)?;
        Ok(())
    }
}

fn lock_path_for(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".lock");
    PathBuf::from(s)
}

/// Run `f` while holding an exclusive interprocess lock on `lock_path`, so a
/// full reload-mutate-persist read-modify-write cycle is atomic across
/// processes (HR-I3).
fn with_exclusive_lock<T>(lock_path: &Path, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut lock = fd_lock::RwLock::new(file);
    let _guard = lock.write()?;
    f()
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
    if !s.len().is_multiple_of(2) {
        return Err(());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|_| ()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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
    fn concurrent_thread_creates_preserve_every_token() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tokens.json");
        TokenStore::open(&path).unwrap();

        let handles: Vec<_> = (0..8)
            .map(|i| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let mut store = TokenStore::open(&path).unwrap();
                    store
                        .create(format!("agent:t{i}"), "worker".into(), vec![], None)
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let store = TokenStore::open(&path).unwrap();
        let metas = store.list();
        assert_eq!(
            metas.len(),
            8,
            "a concurrent create was lost: {} tokens",
            metas.len()
        );
        for i in 0..8 {
            let principal = format!("agent:t{i}");
            assert!(
                metas.iter().any(|m| m.principal == principal),
                "missing token for {principal}"
            );
        }
    }

    /// HR-I3: two OS processes racing to `create` distinct tokens against the
    /// same store must not lose either other's write, proving the file lock
    /// is a real interprocess boundary (J4).
    #[test]
    fn concurrent_process_creates_preserve_every_token() {
        use std::process::Command;

        const WORKER_ENV: &str = "SHOAL_TOKEN_LOCK_TEST_WORKER";
        const PATH_ENV: &str = "SHOAL_TOKEN_LOCK_TEST_PATH";

        if let Some(principal) = std::env::var_os(WORKER_ENV) {
            let path = PathBuf::from(std::env::var_os(PATH_ENV).expect("worker store path"));
            let mut store = TokenStore::open(&path).unwrap();
            let principal = principal.to_string_lossy().into_owned();
            store
                .create(principal, "worker".into(), vec![], None)
                .unwrap();
            return;
        }

        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("tokens.json");
        // Bootstrap the store (and its key) before spawning concurrent writers.
        TokenStore::open(&path).unwrap();

        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for i in 0..12 {
            let principal = format!("agent:worker-{i}");
            children.push(
                Command::new(&exe)
                    .args([
                        "--exact",
                        "tests::concurrent_process_creates_preserve_every_token",
                        "--nocapture",
                    ])
                    .env(WORKER_ENV, &principal)
                    .env(PATH_ENV, &path)
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            assert!(child.wait_with_output().unwrap().status.success());
        }

        let store = TokenStore::open(&path).unwrap();
        let metas = store.list();
        assert_eq!(
            metas.len(),
            12,
            "a concurrent token create was lost: {} tokens",
            metas.len()
        );
        for i in 0..12 {
            let principal = format!("agent:worker-{i}");
            assert!(
                metas.iter().any(|m| m.principal == principal),
                "missing token for {principal}"
            );
        }
    }
}
