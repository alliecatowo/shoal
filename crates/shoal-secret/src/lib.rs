use aes_gcm::{
    Aes256Gcm, KeyInit,
    aead::{Aead, OsRng, rand_core::RngCore},
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

/// Encrypted secret storage. The confidentiality boundary is the containing
/// directory's OS permissions, not the AES-GCM envelope: `master.key` and
/// `secrets.json` are deliberately colocated, so any reader of this directory
/// (same user/process, a directory copy, a backup) recovers both the key and
/// the ciphertext. See "Secret store design" in
/// `site/content/internals/security-threat-model.md` for the full boundary
/// statement and the OS-keyring evaluation/deferral rationale.
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
        if !s.key_path().exists() {
            let mut k = Zeroizing::new([0u8; 32]);
            OsRng.fill_bytes(&mut *k);
            atomic(&s.key_path(), &*k)?
        }
        check_mode(&s.key_path())?;
        Ok(s)
    }
    pub fn set(&self, name: &str, value: &[u8]) -> io::Result<()> {
        valid(name)?;
        let mut m = self.load()?;
        if let Some(old) = m.0.insert(name.into(), value.to_vec()) {
            drop(Zeroizing::new(old));
        }
        self.save(&m)
    }
    pub fn get(&self, name: &str) -> io::Result<Option<Zeroizing<Vec<u8>>>> {
        valid(name)?;
        Ok(self.load()?.0.remove(name).map(Zeroizing::new))
    }
    pub fn list(&self) -> io::Result<Vec<String>> {
        Ok(self.load()?.0.keys().cloned().collect())
    }
    pub fn delete(&self, name: &str) -> io::Result<bool> {
        valid(name)?;
        let mut m = self.load()?;
        let found = m.0.remove(name).map(Zeroizing::new);
        if found.is_some() {
            self.save(&m)?
        }
        Ok(found.is_some())
    }
    fn key_path(&self) -> PathBuf {
        self.dir.join("master.key")
    }
    fn data_path(&self) -> PathBuf {
        self.dir.join("secrets.json")
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
    fn load(&self) -> io::Result<PlainSecrets> {
        if !self.data_path().exists() {
            return Ok(PlainSecrets::default());
        }
        check_mode(&self.data_path())?;
        let e: Envelope = serde_json::from_slice(&fs::read(self.data_path())?).map_err(invalid)?;
        if e.version != 1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported secret store version",
            ));
        }
        let nonce = base64::engine::general_purpose::STANDARD
            .decode(e.nonce)
            .map_err(invalid)?;
        let ct = base64::engine::general_purpose::STANDARD
            .decode(e.ciphertext)
            .map_err(invalid)?;
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let plain = Zeroizing::new(
            cipher
                .decrypt(nonce.as_slice().into(), ct.as_ref())
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "secret store authentication failed",
                    )
                })?,
        );
        serde_json::from_slice(&plain).map_err(invalid)
    }
    fn save(&self, m: &PlainSecrets) -> io::Result<()> {
        let plain = Zeroizing::new(serde_json::to_vec(m).map_err(invalid)?);
        let key = self.key()?;
        let cipher = Aes256Gcm::new_from_slice(&key).map_err(invalid)?;
        let mut nonce = [0u8; 12];
        OsRng.fill_bytes(&mut nonce);
        let ct = cipher
            .encrypt((&nonce).into(), plain.as_ref())
            .map_err(invalid)?;
        let e = Envelope {
            version: 1,
            nonce: base64::engine::general_purpose::STANDARD.encode(nonce),
            ciphertext: base64::engine::general_purpose::STANDARD.encode(ct),
        };
        atomic(&self.data_path(), &serde_json::to_vec(&e).map_err(invalid)?)
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
fn atomic(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path.parent().unwrap();
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
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
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
}
