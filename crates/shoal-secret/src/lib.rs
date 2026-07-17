use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

/// Encrypted secret storage whose confidentiality boundary is the containing
/// directory's OS permissions. `master.key` and `secrets.json` are deliberately
/// colocated, so copying that directory copies both key and ciphertext; this is
/// encrypted-at-rest hygiene, not protection from an actor that can read the
/// store directory.
#[derive(Clone)]
pub struct SecretStore {
    dir: PathBuf,
}
impl std::fmt::Debug for SecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}
#[derive(Serialize, Deserialize)]
struct Envelope {
    version: u8,
    nonce: String,
    ciphertext: String,
}

/// Plaintext map with deterministic zeroization of every value on all exits,
/// including parse/save errors and replacement/deletion paths.
#[derive(Default, Serialize, Deserialize)]
#[serde(transparent)]
struct PlainSecrets(BTreeMap<String, Vec<u8>>);

impl Drop for PlainSecrets {
    fn drop(&mut self) {
        for value in self.0.values_mut() {
            value.zeroize();
        }
    }
}

impl SecretStore {
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let s = Self { dir: dir.into() };
        fs::create_dir_all(&s.dir)?;
        secure_dir(&s.dir)?;
        // Key creation must share the same interprocess transaction lock as
        // map updates: two first-openers must never install different keys.
        s.with_exclusive_lock(|| {
            if !s.key_path().exists() {
                let mut k = Zeroizing::new([0u8; 32]);
                OsRng.fill_bytes(&mut *k);
                atomic(&s.key_path(), &*k)?
            }
            check_mode(&s.key_path())
        })?;
        Ok(s)
    }
    pub fn set(&self, name: &str, value: &[u8]) -> io::Result<()> {
        valid(name)?;
        self.with_exclusive_lock(|| {
            let mut m = self.load_unlocked()?;
            if let Some(old) = m.0.insert(name.into(), value.to_vec()) {
                drop(Zeroizing::new(old));
            }
            self.save_unlocked(&m)
        })
    }
    pub fn get(&self, name: &str) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        valid(name)?;
        self.with_shared_lock(|| Ok(self.load_unlocked()?.0.remove(name).map(Zeroizing::new)))
    }
    pub fn list(&self) -> io::Result<Vec<String>> {
        self.with_shared_lock(|| Ok(self.load_unlocked()?.0.keys().cloned().collect()))
    }
    pub fn delete(&self, name: &str) -> io::Result<bool> {
        valid(name)?;
        self.with_exclusive_lock(|| {
            let mut m = self.load_unlocked()?;
            let found = m.0.remove(name).map(Zeroizing::new);
            if found.is_some() {
                self.save_unlocked(&m)?
            }
            Ok(found.is_some())
        })
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("master.key")
    }
    fn data_path(&self) -> PathBuf {
        self.dir.join("secrets.json")
    }
    fn lock_path(&self) -> PathBuf {
        self.dir.join(".secrets.lock")
    }
    fn key(&self) -> io::Result<Zeroizing<Vec<u8>>> {
        let k = Zeroizing::new(fs::read(self.key_path())?);
        if k.len() != 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid secret key length",
            ));
        }
        Ok(k)
    }
    fn load_unlocked(&self) -> io::Result<PlainSecrets> {
        if !self.data_path().exists() {
            return Ok(PlainSecrets::default());
        }
        check_mode(&self.data_path())?;
        let envelope_bytes = Zeroizing::new(fs::read(self.data_path())?);
        let e: Envelope = serde_json::from_slice(&envelope_bytes).map_err(invalid)?;
        if e.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported secret store version",
            ));
        }
        let nonce_bytes = base64::engine::general_purpose::STANDARD
            .decode(e.nonce)
            .map_err(invalid)?;
        let nonce: [u8; 12] = nonce_bytes.as_slice().try_into().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid secret store nonce length",
            )
        })?;
        let ct = Zeroizing::new(
            base64::engine::general_purpose::STANDARD
                .decode(e.ciphertext)
                .map_err(invalid)?,
        );
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let plain = Zeroizing::new(cipher.decrypt((&nonce).into(), ct.as_ref()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "secret store authentication failed",
            )
        })?);
        serde_json::from_slice(&plain).map_err(invalid)
    }
    fn save_unlocked(&self, m: &PlainSecrets) -> io::Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(m).map_err(invalid)?);
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = Zeroizing::new(
            cipher
                .encrypt((&nonce).into(), plain.as_ref())
                .map_err(invalid)?,
        );
        let e = Envelope {
            version: 1,
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ct.as_slice()),
        };
        atomic(&self.data_path(), &serde_json::to_vec(&e).map_err(invalid)?)
    }

    fn with_exclusive_lock<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let file = open_lock_file(&self.lock_path())?;
        let mut lock = fd_lock::RwLock::new(file);
        let _guard = lock.write()?;
        f()
    }

    fn with_shared_lock<T>(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<T> {
        let file = open_lock_file(&self.lock_path())?;
        let lock = fd_lock::RwLock::new(file);
        let _guard = lock.read()?;
        f()
    }
}
fn valid(n: &str) -> io::Result<()> {
    if n.is_empty()
        || !n
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "secret name must contain only ASCII letters, digits, _ or -",
        ))
    } else {
        Ok(())
    }
}
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
#[cfg(unix)]
fn secure_dir(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))
}
#[cfg(not(unix))]
fn secure_dir(_: &Path) -> io::Result<()> {
    Ok(())
}
#[cfg(unix)]
fn check_mode(p: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if fs::metadata(p)?.permissions().mode() & 0o077 != 0 {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "secret file permissions must be 0600",
        ))
    } else {
        Ok(())
    }
}
#[cfg(not(unix))]
fn check_mode(_: &Path) -> io::Result<()> {
    Ok(())
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "secret store file needs a parent directory",
        )
    })?;
    let mut t = tempfile::NamedTempFile::new_in(parent)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        t.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?
    }
    io::Write::write_all(&mut t, data)?;
    t.as_file().sync_all()?;
    t.persist(path).map_err(|e| e.error)?;
    // Persist the directory entry as well as the temporary file contents.
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use std::sync::Arc;

    #[test]
    fn lifecycle_and_redaction() {
        let t = tempfile::tempdir().unwrap();
        let s = SecretStore::open(t.path()).unwrap();
        s.set("TOKEN", b"super-secret").unwrap();
        assert_eq!(&*s.get("TOKEN").unwrap().unwrap(), b"super-secret");
        assert_eq!(s.list().unwrap(), ["TOKEN"]);
        let disk = fs::read_to_string(t.path().join("secrets.json")).unwrap();
        assert!(!disk.contains("super-secret"));
        assert!(!format!("{s:?}").contains("super-secret"));
        assert!(s.delete("TOKEN").unwrap());
        assert!(s.get("TOKEN").unwrap().is_none())
    }
    #[test]
    fn tamper_detected() {
        let t = tempfile::tempdir().unwrap();
        let s = SecretStore::open(t.path()).unwrap();
        s.set("A", b"x").unwrap();
        let p = t.path().join("secrets.json");
        let mut b = fs::read(&p).unwrap();
        let n = b.len();
        b[n - 2] ^= 1;
        fs::write(p, b).unwrap();
        assert!(s.list().is_err())
    }

    #[test]
    fn malformed_nonce_is_typed_under_repeated_concurrent_access() {
        let t = tempfile::tempdir().unwrap();
        let store = Arc::new(SecretStore::open(t.path()).unwrap());
        store.set("TOKEN", b"authority").unwrap();
        let path = t.path().join("secrets.json");
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        envelope["nonce"] =
            serde_json::Value::String(base64::engine::general_purpose::STANDARD.encode([0u8; 11]));
        fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();

        for _ in 0..8 {
            assert_eq!(store.list().unwrap_err().kind(), io::ErrorKind::InvalidData);
            assert_eq!(
                store.get("TOKEN").unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                store.set("TOKEN", b"replacement").unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            assert_eq!(
                store.delete("TOKEN").unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }

        let workers: Vec<_> = (0..8)
            .map(|_| {
                let store = Arc::clone(&store);
                std::thread::spawn(move || {
                    for _ in 0..16 {
                        assert!(store.list().is_err());
                        assert!(store.get("TOKEN").is_err());
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().expect("secret-store worker must not panic");
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
                "production secret synchronization must return typed errors: {forbidden}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn store_permissions_are_the_confidentiality_boundary() {
        use std::os::unix::fs::PermissionsExt;

        let t = tempfile::tempdir().unwrap();
        let s = SecretStore::open(t.path()).unwrap();
        s.set("TOKEN", b"secret").unwrap();
        assert_eq!(
            fs::metadata(t.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in ["master.key", "secrets.json", ".secrets.lock"] {
            assert_eq!(
                fs::metadata(t.path().join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600,
                "{name} must be owner-only"
            );
        }
        // The encrypted file omits plaintext, but the same protected directory
        // also contains its decryption key: OS ownership/mode is the boundary.
        assert!(t.path().join("master.key").exists());
        assert!(t.path().join("secrets.json").exists());
    }

    #[test]
    fn concurrent_process_writers_preserve_every_secret() {
        const WORKER_ENV: &str = "SHOAL_SECRET_LOCK_TEST_WORKER";
        const DIR_ENV: &str = "SHOAL_SECRET_LOCK_TEST_DIR";

        if let Some(name) = std::env::var_os(WORKER_ENV) {
            let dir = PathBuf::from(std::env::var_os(DIR_ENV).expect("worker store dir"));
            let store = SecretStore::open(dir).unwrap();
            let name = name.to_string_lossy();
            store.set(&name, name.as_bytes()).unwrap();
            return;
        }

        let t = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let mut children = Vec::new();
        for i in 0..12 {
            let name = format!("SECRET_{i}");
            children.push(
                Command::new(&exe)
                    .args([
                        "--exact",
                        "tests::concurrent_process_writers_preserve_every_secret",
                        "--nocapture",
                    ])
                    .env(WORKER_ENV, &name)
                    .env(DIR_ENV, t.path())
                    .spawn()
                    .unwrap(),
            );
        }
        for child in children {
            assert!(child.wait_with_output().unwrap().status.success());
        }

        let store = SecretStore::open(t.path()).unwrap();
        let names = store.list().unwrap();
        assert_eq!(names.len(), 12, "a concurrent update was lost: {names:?}");
        for i in 0..12 {
            let name = format!("SECRET_{i}");
            assert_eq!(&*store.get(&name).unwrap().unwrap(), name.as_bytes());
        }
    }
}
